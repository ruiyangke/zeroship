use super::*;
use crate::auth_token::{anchor_aad, resolve_route, rotate_family, RouteCtx};
use futures::{poll, FutureExt};

async fn stored_anchor(state: &GateState, user: &UserId) -> anchors::Anchor {
    let encrypted = zeroship_core::crypto::encrypt(
        &state.anchor_enc_key,
        &anchor_aad(client_id(), user.as_str()),
        INITIAL_REFRESH_TOKEN.as_bytes(),
    )
    .unwrap();
    let pool = crate::db::checkout(state.db.as_ref().unwrap())
        .await
        .unwrap();
    let mut connection = pool.acquire().await.unwrap();
    anchors::create(
        &mut connection,
        &anchors::NewAnchor {
            app_id: &AppId::parse(APP_ID).unwrap(),
            client_id: client_id(),
            global_user_id: user,
            refresh_token_enc: &encrypted,
            refresh_family_id: "rotation-fixture",
            granted_scopes: &["openid".to_owned(), "email".to_owned()],
        },
    )
    .await
    .unwrap()
}

fn route(state: &GateState) -> RouteCtx {
    resolve_route(
        &test::TestRequest::get()
            .header("host", APP_HOST)
            .to_http_request(),
        state,
    )
    .unwrap_or_else(|response| panic!("fixture route: {}", response.status()))
}

#[ntex::test]
async fn concurrent_callers_share_the_real_rotation_and_persist_its_result() {
    Database::migrated(async |database| {
        let op = Arc::new(MockOP::new(client_id()));
        op.enforce_refresh_reuse_detection();
        seed_user(&database.admin, &op.user_id).await;
        let (base, _server) = boot_mock_op(op.clone()).await;
        let (state, _files) = build_state(&base, Some(database.config_as("zeroship_gateway", 1)));
        let anchor = stored_anchor(&state, &op.user_id).await;
        let route = route(&state);
        let (entered, release) = op.pause_refresh();
        let mut leader = Box::pin(rotate_family(&state, &route, &anchor));
        futures::select_biased! {
            result = leader.as_mut().fuse() => panic!("rotation finished before release: {result:?}"),
            reached = entered.recv_async().fuse() => reached.unwrap(),
        }
        let mut followers: Vec<_> = (0..8)
            .map(|_| Box::pin(rotate_family(&state, &route, &anchor)))
            .collect();
        for follower in &mut followers {
            assert!(poll!(follower.as_mut()).is_pending());
        }
        // The gateway pool remains usable while the provider is paused.
        let pool = crate::db::checkout(state.db.as_ref().unwrap()).await.unwrap();
        let lease = pool.acquire().await.unwrap();
        assert_eq!(lease.query_one("SELECT 1", &[]).await.unwrap().get::<_, i32>(0), 1);
        drop(lease);
        release.send(()).unwrap();
        followers.push(leader);
        for result in futures::future::join_all(followers).await {
            let result = result.expect("coalesced rotation");
            assert_eq!(result.global_user_id, op.user_id);
            assert_eq!(result.name.as_deref(), Some(ROTATED_NAME));
        }
        assert_eq!(op.presented_refresh_tokens(), [INITIAL_REFRESH_TOKEN]);
        assert_eq!(op.refresh_calls.load(Ordering::SeqCst), 1);
        assert_eq!(anchors::with_single_flight(|flight| flight.in_flight()), 0);

        let mut connection = pool.acquire().await.unwrap();
        let rotated = anchors::read_live(&mut connection, &anchor.app_id, anchor.id)
            .await.unwrap().unwrap();
        drop(connection);
        rotate_family(&state, &route, &rotated).await.expect("later rotation runs again");
        assert_eq!(op.presented_refresh_tokens(), [INITIAL_REFRESH_TOKEN, "rt_rotated_1"]);
    }).await;
}

#[ntex::test]
async fn a_follower_finishes_the_real_rotation_after_the_leader_disconnects() {
    Database::migrated(async |database| {
        let op = Arc::new(MockOP::new(client_id()));
        seed_user(&database.admin, &op.user_id).await;
        let (base, _server) = boot_mock_op(op.clone()).await;
        let (state, _files) = build_state(&base, Some(database.config_as("zeroship_gateway", 1)));
        let anchor = stored_anchor(&state, &op.user_id).await;
        let route = route(&state);
        let (entered, release) = op.pause_refresh();
        let mut leader = Box::pin(rotate_family(&state, &route, &anchor));
        futures::select_biased! {
            result = leader.as_mut().fuse() => panic!("rotation finished before release: {result:?}"),
            reached = entered.recv_async().fuse() => reached.unwrap(),
        }
        let mut follower = Box::pin(rotate_family(&state, &route, &anchor));
        assert!(poll!(follower.as_mut()).is_pending());
        drop(leader);
        assert!(anchors::with_single_flight(|flight| flight.get(anchor.id)).is_some());
        release.send(()).unwrap();
        assert_eq!(follower.await.expect("follower completes").global_user_id, op.user_id);
        assert_eq!(op.refresh_calls.load(Ordering::SeqCst), 1);
        assert_eq!(anchors::with_single_flight(|flight| flight.in_flight()), 0);
    }).await;
}

#[ntex::test]
async fn disconnecting_every_caller_releases_the_real_rotation() {
    Database::migrated(async |database| {
        let op = Arc::new(MockOP::new(client_id()));
        seed_user(&database.admin, &op.user_id).await;
        let (base, _server) = boot_mock_op(op.clone()).await;
        let (state, _files) = build_state(&base, Some(database.config_as("zeroship_gateway", 1)));
        let anchor = stored_anchor(&state, &op.user_id).await;
        let route = route(&state);
        let (entered, release) = op.pause_refresh();
        let mut leader = Box::pin(rotate_family(&state, &route, &anchor));
        futures::select_biased! {
            result = leader.as_mut().fuse() => panic!("rotation finished before release: {result:?}"),
            reached = entered.recv_async().fuse() => reached.unwrap(),
        }
        let mut follower = Box::pin(rotate_family(&state, &route, &anchor));
        assert!(poll!(follower.as_mut()).is_pending());
        drop(leader);
        drop(follower);
        let retained = anchors::with_single_flight(|flight| flight.get(anchor.id));
        let leaked = retained.is_some();
        let in_flight = anchors::with_single_flight(|flight| flight.in_flight());
        // Clean up only after observing, keeping regression failures readable.
        anchors::with_single_flight(|flight| flight.remove(anchor.id));
        drop(retained);
        release.send(()).unwrap();
        assert!(!leaked, "abandoned rotation body is still retained");
        assert_eq!(in_flight, 0);
    }).await;
}
