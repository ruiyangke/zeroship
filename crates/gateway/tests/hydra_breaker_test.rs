//! Faithful integration tests for the shared, breaker-guarded OP client
//! (auth-sdk §8.7, round-6 MAJOR #3).
//!
//! These drive the REAL `OidcRp` token path (`refresh_token_public` →
//! `post_token` → `hydra_client::call`) against a loopback MOCK OP (an
//! in-process ntex test server), exactly like `auth_token_anchors_test.rs`.
//! Nothing about the breaker, the reused client, or the bounded timeout is
//! stubbed — the only fake is the OP itself, which:
//!
//! - counts EVERY `/token` request it receives, so "fast-fail while
//!   open ⇒ the mock sees no further call" is an exact assertion;
//! - can be flipped to answer `400 invalid_grant` (a valid upstream response,
//!   NOT a breaker failure);
//! - can be flipped to add an artificial delay (so a short bounded timeout
//!   fires and counts as a breaker failure);
//! - signs nothing it doesn't need to — the breaker tests assert on
//!   `OidcRpError` shape + the mock's call counter, not on token contents.
//!
//! Connection-refused failures (the "transport error" breaker-failure class)
//! are produced by pointing the `OidcRp` at a dead loopback port.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_gateway::hydra_client::{BreakerState, CircuitBreaker};
use zeroship_gateway::oidc_rp::{BrokerSecret, OidcRp, OidcRpError};

const CLIENT_ID: &str = "oac_myapp";
const TEST_BROKER_MASTER: &[u8] = b"gateway-breaker-test-broker-master-32-bytes";

/// Minimal mock OP `/token`. Counts every request, and can be
/// flipped to answer `invalid_grant` or to add an artificial delay.
#[derive(Default)]
struct MockHydra {
    /// Total `/token` requests received (the fast-fail proof).
    calls: AtomicU32,
    /// When true, answer `400 invalid_grant` (a valid upstream response).
    invalid_grant: AtomicBool,
    /// Artificial delay (ms) before answering — fires the bounded timeout.
    delay_ms: AtomicU32,
}

async fn token_endpoint(
    _body: ntex::util::Bytes,
    h: web::types::State<Arc<MockHydra>>,
) -> web::HttpResponse {
    h.calls.fetch_add(1, Ordering::SeqCst);
    let delay = h.delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        ntex::time::sleep(Duration::from_millis(u64::from(delay))).await;
    }
    if h.invalid_grant.load(Ordering::SeqCst) {
        return web::HttpResponse::BadRequest()
            .header("content-type", "application/json")
            .body(r#"{"error":"invalid_grant","error_description":"token expired"}"#);
    }
    // A valid TokenSet body so the happy path deserializes cleanly.
    web::HttpResponse::Ok()
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "access_token": "at_ok",
                "refresh_token": format!("rt_{}", Uuid::new_v4().simple()),
                "token_type": "Bearer",
                "expires_in": 3600,
                "scope": "openid email profile offline_access",
            })
            .to_string(),
        )
}

/// Boot the loopback mock OP. Returns `(base_url, server)`.
async fn boot_mock(hydra: Arc<MockHydra>) -> (String, test::TestServer) {
    let srv = test::server(move || {
        let h = hydra.clone();
        async move {
            web::App::new()
                .state(h)
                .service(web::resource("/token").route(web::post().to(token_endpoint)))
        }
    })
    .await;
    let base = srv.url("").trim_end_matches('/').to_string();
    (base, srv)
}

/// Build an `OidcRp` dialing `base`, with an explicit fast-tripping breaker
/// and a chosen per-call timeout, so the state-machine transitions are quick.
fn rp_with(base: &str, breaker: Arc<CircuitBreaker>, timeout: Duration) -> OidcRp {
    OidcRp::new(
        base,
        BrokerSecret::from_bytes(TEST_BROKER_MASTER.to_vec()).expect("broker secret"),
        b"k".repeat(32),
    )
        .with_breaker(breaker)
        .with_hydra_timeout(timeout)
}

// ─── 1. The client is REUSED across calls (breaker state persists) ─────────

#[ntex::test]
async fn client_is_reused_breaker_state_persists_across_calls() {
    // Three successful refreshes through ONE OidcRp. If the client were
    // reconstructed per call the breaker would still work, so the tighter
    // claim is: the SAME shared breaker is consulted every call and stays
    // closed across all three — i.e. one persistent state object, not a fresh
    // one per call. We also assert the mock saw exactly three calls (no retry
    // storm, no dropped call) — the reused-client happy path is unchanged.
    let hydra = Arc::new(MockHydra::default());
    let (base, _srv) = boot_mock(hydra.clone()).await;
    let breaker = Arc::new(CircuitBreaker::new(3, Duration::from_millis(50)));
    let rp = rp_with(&base, breaker.clone(), Duration::from_secs(5));

    for _ in 0..3 {
        let out = rp.refresh_token_public(CLIENT_ID, "rt_seed").await;
        assert!(out.is_ok(), "happy-path refresh should succeed: {out:?}");
        // The shared breaker is consulted and stays closed each time.
        assert_eq!(breaker.state(), BreakerState::Closed);
    }
    assert_eq!(
        hydra.calls.load(Ordering::SeqCst),
        3,
        "exactly one upstream call per refresh — success path unchanged"
    );
}

// ─── 2. Breaker OPENS after N failures, then fast-fails without hitting Hydra ─

#[ntex::test]
async fn breaker_opens_after_n_failures_then_fast_fails_without_calling_hydra() {
    // Drive N connection-refused failures by pointing at a DEAD port, then
    // assert the breaker is open and the next call fast-fails WITHOUT any
    // network attempt. To prove "no Hydra contact while open" exactly, the
    // breaker is shared with a SECOND OidcRp that points at a LIVE mock: once
    // the breaker is open, a call through the live-mock RP must NOT increment
    // the mock's counter (the breaker short-circuits before the request).
    let hydra = Arc::new(MockHydra::default());
    let (live_base, _srv) = boot_mock(hydra.clone()).await;

    // One shared breaker, two RPs: one dials a dead port (to rack up
    // failures), one dials the live mock (to prove the open breaker blocks
    // real traffic). This is the multi-thread brownout shape in miniature —
    // shared breaker state across independent call sites.
    let breaker = Arc::new(CircuitBreaker::new(3, Duration::from_secs(30)));
    let dead = rp_with("http://127.0.0.1:1", breaker.clone(), Duration::from_secs(2));
    let live = rp_with(&live_base, breaker.clone(), Duration::from_secs(2));

    // 3 consecutive transport failures (connection refused on port 1).
    for i in 0..3 {
        let err = dead
            .refresh_token_public(CLIENT_ID, "rt")
            .await
            .expect_err("dead-port refresh must fail");
        // Failures while closed surface as a token-exchange transport error,
        // NOT upstream_unavailable (that's the open/timeout signal).
        if i < 2 {
            assert!(
                matches!(err, OidcRpError::TokenExchange(_)),
                "pre-trip failure is a transport error: {err:?}"
            );
        }
    }
    assert_eq!(breaker.state(), BreakerState::Open, "breaker must be open after 3 failures");

    // Now a call through the LIVE-mock RP must fast-fail WITHOUT touching the
    // mock — the shared open breaker short-circuits it.
    let before = hydra.calls.load(Ordering::SeqCst);
    let err = live
        .refresh_token_public(CLIENT_ID, "rt")
        .await
        .expect_err("open breaker must fast-fail");
    assert!(
        err.is_upstream_unavailable(),
        "open breaker fast-fails with upstream_unavailable: {err:?}"
    );
    let after = hydra.calls.load(Ordering::SeqCst);
    assert_eq!(
        before, after,
        "while OPEN the breaker must NOT issue any Hydra request (no mock call)"
    );
}

// ─── 3. Recovery: half-open probe success closes the breaker ───────────────

#[ntex::test]
async fn half_open_probe_success_recovers_the_breaker() {
    // Open the breaker on a dead port, wait out a short cooldown, then a
    // probe through the LIVE mock succeeds and closes the breaker so normal
    // traffic flows again.
    let hydra = Arc::new(MockHydra::default());
    let (live_base, _srv) = boot_mock(hydra.clone()).await;
    let breaker = Arc::new(CircuitBreaker::new(2, Duration::from_millis(80)));
    let dead = rp_with("http://127.0.0.1:1", breaker.clone(), Duration::from_secs(2));
    let live = rp_with(&live_base, breaker.clone(), Duration::from_secs(2));

    for _ in 0..2 {
        let _ = dead.refresh_token_public(CLIENT_ID, "rt").await;
    }
    assert_eq!(breaker.state(), BreakerState::Open);

    // Before cooldown elapses, a live call still fast-fails.
    let err = live.refresh_token_public(CLIENT_ID, "rt").await.expect_err("still open");
    assert!(err.is_upstream_unavailable(), "{err:?}");
    let calls_while_open = hydra.calls.load(Ordering::SeqCst);
    assert_eq!(calls_while_open, 0, "no mock traffic while open");

    // Wait out the cooldown; the next live call is admitted as the probe and
    // succeeds → breaker closes.
    ntex::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(breaker.state(), BreakerState::HalfOpen, "cooldown elapsed ⇒ half-open");
    let out = live.refresh_token_public(CLIENT_ID, "rt").await;
    assert!(out.is_ok(), "half-open probe should reach the mock and succeed: {out:?}");
    assert_eq!(breaker.state(), BreakerState::Closed, "probe success closes the breaker");
    assert_eq!(hydra.calls.load(Ordering::SeqCst), 1, "exactly the one probe reached Hydra");

    // Breaker fully usable again — a follow-up call flows normally.
    let out2 = live.refresh_token_public(CLIENT_ID, "rt").await;
    assert!(out2.is_ok());
    assert_eq!(hydra.calls.load(Ordering::SeqCst), 2);
}

// ─── 4. A Hydra 4xx does NOT trip the breaker ──────────────────────────────

#[ntex::test]
async fn hydra_4xx_invalid_grant_does_not_trip_the_breaker() {
    // The mock answers 400 invalid_grant for EVERY call. invalid_grant is a
    // valid upstream response (the token is dead), NOT a transport failure —
    // so even many of them must leave the breaker CLOSED. The caller still
    // gets a TokenExchange error carrying the upstream body (so the
    // invalid_grant substring detection upstream keeps working), but the
    // breaker never opens and never fast-fails.
    let hydra = Arc::new(MockHydra::default());
    hydra.invalid_grant.store(true, Ordering::SeqCst);
    let (base, _srv) = boot_mock(hydra.clone()).await;
    // Threshold 3 — but invalid_grant must never count toward it.
    let breaker = Arc::new(CircuitBreaker::new(3, Duration::from_secs(30)));
    let rp = rp_with(&base, breaker.clone(), Duration::from_secs(5));

    for _ in 0..5 {
        let err = rp
            .refresh_token_public(CLIENT_ID, "rt")
            .await
            .expect_err("invalid_grant surfaces as an error");
        // It's a token-exchange error carrying the upstream body, NOT an
        // upstream_unavailable fast-fail.
        match &err {
            OidcRpError::TokenExchange(m) => {
                assert!(m.contains("invalid_grant"), "carries upstream body: {m}");
            }
            other => panic!("expected TokenExchange(invalid_grant), got {other:?}"),
        }
        assert!(!err.is_upstream_unavailable());
    }
    assert_eq!(
        breaker.state(),
        BreakerState::Closed,
        "5 consecutive 4xx invalid_grant must NOT trip the breaker"
    );
    assert_eq!(
        hydra.calls.load(Ordering::SeqCst),
        5,
        "every call reached Hydra — none was fast-failed by the breaker"
    );
}

// ─── 5. A hung/slow upstream hits the bounded timeout (counts as a failure) ─

#[ntex::test]
async fn slow_upstream_hits_bounded_timeout_and_counts_as_failure() {
    // The mock delays every answer well past the RP's bounded timeout. Each
    // call must return a fast upstream_unavailable (NOT hang for the full
    // delay), and the timeouts count as breaker failures — so after the
    // threshold the breaker opens and subsequent calls fast-fail.
    let hydra = Arc::new(MockHydra::default());
    hydra.delay_ms.store(1_000, Ordering::SeqCst); // 1s upstream delay
    let (base, _srv) = boot_mock(hydra.clone()).await;
    let breaker = Arc::new(CircuitBreaker::new(2, Duration::from_secs(30)));
    // 100ms bounded timeout ≪ 1s upstream delay.
    let rp = rp_with(&base, breaker.clone(), Duration::from_millis(100));

    // First two calls time out (counted failures) and trip the breaker.
    for _ in 0..2 {
        let start = std::time::Instant::now();
        let err = rp
            .refresh_token_public(CLIENT_ID, "rt")
            .await
            .expect_err("slow upstream must time out");
        assert!(
            err.is_upstream_unavailable(),
            "bounded-timeout surfaces as upstream_unavailable: {err:?}"
        );
        assert!(
            start.elapsed() < Duration::from_millis(800),
            "must fail FAST at the bounded timeout, not wait the full 1s upstream delay (took {:?})",
            start.elapsed()
        );
    }
    assert_eq!(breaker.state(), BreakerState::Open, "two timeouts must trip the breaker");

    // The breaker is now open: the next call fast-fails WITHOUT a new request.
    let calls_before = hydra.calls.load(Ordering::SeqCst);
    let err = rp.refresh_token_public(CLIENT_ID, "rt").await.expect_err("open");
    assert!(err.is_upstream_unavailable(), "{err:?}");
    assert_eq!(
        hydra.calls.load(Ordering::SeqCst),
        calls_before,
        "open breaker issues no further upstream request"
    );
}

// ─── 6. A DROPPED half-open probe does NOT wedge the breaker ───────────────

#[ntex::test]
async fn dropped_half_open_probe_does_not_wedge_the_breaker() {
    // Regression for the probe-slot leak (round-7 BLOCKER). The mint hot path
    // drives a Hydra refresh inside a CANCELLABLE single-flight future; when the
    // breaker is HalfOpen and the sole leader is cancelled mid-probe (client
    // disconnect / ntex timeout, no follower), the probe slot must NOT leak —
    // otherwise every later admit() CAS fails and the breaker self-DoSes all
    // gateway→Hydra traffic until restart.
    //
    // We reproduce that exactly: open the breaker, wait out the cooldown so the
    // next call is admitted as the probe, then DROP that probe future while it
    // is parked on a slow upstream (await never resolves on our side). The
    // ProbeGuard's Drop must release the slot + re-arm the cooldown. A
    // subsequent call, after the cooldown, must then be ADMITTED (and succeed),
    // not rejected forever.
    let hydra = Arc::new(MockHydra::default());
    let (live_base, _srv) = boot_mock(hydra.clone()).await;
    let breaker = Arc::new(CircuitBreaker::new(2, Duration::from_millis(80)));
    let dead = rp_with("http://127.0.0.1:1", breaker.clone(), Duration::from_secs(2));
    // Generous per-call timeout so the probe future does NOT self-resolve via
    // the bounded timeout before we drop it — the drop is what we're testing.
    let live = rp_with(&live_base, breaker.clone(), Duration::from_secs(30));

    // Open the breaker (2 transport failures on the dead port).
    for _ in 0..2 {
        let _ = dead.refresh_token_public(CLIENT_ID, "rt").await;
    }
    assert_eq!(breaker.state(), BreakerState::Open);

    // Wait out the cooldown ⇒ next live call is admitted as the single probe.
    ntex::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(breaker.state(), BreakerState::HalfOpen, "cooldown elapsed ⇒ half-open");

    // Make the mock hang so the probe future is genuinely in flight when we
    // drop it (parked on the upstream await, slot claimed).
    hydra.delay_ms.store(5_000, Ordering::SeqCst);
    {
        let probe = live.refresh_token_public(CLIENT_ID, "rt");
        // Race the probe against an immediate timeout, then DROP it: this is
        // the cancellation the mint single-flight leader undergoes.
        let dropped = ntex::time::timeout(Duration::from_millis(50), probe).await;
        assert!(dropped.is_err(), "probe must still be in flight when we drop it");
        // `dropped` (the Err) holds nothing; the inner `probe` future has been
        // dropped here by `timeout` — ProbeGuard::drop fired.
    }
    // Stop the mock hanging so the recovery probe can complete promptly.
    hydra.delay_ms.store(0, Ordering::SeqCst);

    // The slot must be released and the cooldown re-armed (Open, not a wedged
    // HalfOpen). Wait out the fresh cooldown and prove a probe is admittable.
    ntex::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        breaker.state(),
        BreakerState::HalfOpen,
        "abandoned probe re-armed the cooldown ⇒ half-open again (NOT wedged shut)",
    );
    let out = live.refresh_token_public(CLIENT_ID, "rt").await;
    assert!(
        out.is_ok(),
        "after a dropped probe the breaker must admit + recover on the next probe, not reject forever: {out:?}",
    );
    assert_eq!(breaker.state(), BreakerState::Closed, "recovery probe closes the breaker");
}

// ─── 7. Two concurrent minters ⇒ exactly ONE half-open probe reaches Hydra ──

#[ntex::test]
async fn concurrent_half_open_probes_admit_exactly_one() {
    // The "exactly one probe" CAS is asserted in the unit test via back-to-back
    // synchronous admit() calls; here we prove it holds across genuinely
    // concurrent awaited tasks racing into a HalfOpen breaker. Two refreshes are
    // launched at once against a slow mock; the shared breaker must admit only
    // ONE as the probe (the other fast-fails with upstream_unavailable), so the
    // mock sees exactly one request — no thundering herd into a recovering
    // upstream.
    let hydra = Arc::new(MockHydra::default());
    // Small delay so both tasks are concurrently in flight when the probe is
    // claimed (the loser must observe the slot already taken).
    hydra.delay_ms.store(150, Ordering::SeqCst);
    let (live_base, _srv) = boot_mock(hydra.clone()).await;
    let breaker = Arc::new(CircuitBreaker::new(2, Duration::from_millis(80)));
    let dead = rp_with("http://127.0.0.1:1", breaker.clone(), Duration::from_secs(2));
    let live = Arc::new(rp_with(&live_base, breaker.clone(), Duration::from_secs(5)));

    // Open the breaker, then wait out the cooldown ⇒ HalfOpen.
    for _ in 0..2 {
        let _ = dead.refresh_token_public(CLIENT_ID, "rt").await;
    }
    assert_eq!(breaker.state(), BreakerState::Open);
    ntex::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(breaker.state(), BreakerState::HalfOpen);

    // Launch two refreshes concurrently into the HalfOpen breaker.
    let (a, b) = {
        let l1 = live.clone();
        let l2 = live.clone();
        futures::future::join(
            async move { l1.refresh_token_public(CLIENT_ID, "rt").await },
            async move { l2.refresh_token_public(CLIENT_ID, "rt").await },
        )
        .await
    };

    // Exactly one was admitted as the probe (Ok); the other fast-failed
    // (upstream_unavailable) — the CAS let exactly one through.
    let oks = usize::from(a.is_ok()) + usize::from(b.is_ok());
    let fast_fails = usize::from(a.as_ref().err().is_some_and(OidcRpError::is_upstream_unavailable))
        + usize::from(b.as_ref().err().is_some_and(OidcRpError::is_upstream_unavailable));
    assert_eq!(oks, 1, "exactly one concurrent probe is admitted: a={a:?} b={b:?}");
    assert_eq!(fast_fails, 1, "the other concurrent caller fast-fails: a={a:?} b={b:?}");
    assert_eq!(
        hydra.calls.load(Ordering::SeqCst),
        1,
        "the mock sees exactly ONE probe request — no herd into the recovering upstream",
    );
    assert_eq!(breaker.state(), BreakerState::Closed, "the single probe success closes the breaker");
}
