use super::*;
use std::time::Duration;
use zeroship_core::workflow_policy::AppPolicy;

fn cache(capacity: usize) -> Cache {
    Cache::new(NonZeroUsize::new(capacity).unwrap())
}
fn observation(app: &AppId) -> PolicyObservation {
    PolicyObservation::new(
        app.clone(),
        1.try_into().unwrap(),
        AppPolicy::default(),
        zeroship_core::ZoneId::default_zone(),
        false,
        Instant::now() + Duration::from_secs(30),
    )
    .unwrap()
}
fn reserve<'a>(cache: &'a Cache, app: &AppId) -> Ticket<'a> {
    match cache.reserve(app).unwrap() {
        Reservation::Refresh(ticket) => ticket,
        Reservation::Cached(_) | Reservation::Ahead(..) | Reservation::Wait(_) => {
            panic!("expected an authoritative refresh")
        }
    }
}

/// Whether `observation` is the one this cache answers its app with now, read
/// through the reservation every caller takes. A reservation that would read
/// the source is released unread.
fn serves(cache: &Cache, observation: &PolicyObservation) -> bool {
    match cache.reserve(observation.app_id()).unwrap() {
        Reservation::Cached(current) | Reservation::Ahead(_, current) => {
            current.same_observation(observation)
        }
        Reservation::Refresh(_) | Reservation::Wait(_) => false,
    }
}

#[test]
fn cached_requests_preserve_identity_and_original_deadline() {
    let cache = cache(2);
    let app = AppId::mint();
    let original = reserve(&cache, &app).complete(observation(&app)).unwrap();
    for _ in 0..3 {
        let Reservation::Cached(cached) = cache.reserve(&app).unwrap() else {
            panic!("cached value must not reread the source")
        };
        assert!(cached.same_observation(&original));
        assert_eq!(cached.expires_at(), original.expires_at());
    }
}

#[test]
fn a_concurrent_miss_waits_for_the_inflight_refresh_and_a_cancel_releases_it() {
    let cache = cache(3);
    let app = AppId::mint();
    let other = AppId::mint();
    let peer = reserve(&cache, &other)
        .complete(observation(&other))
        .unwrap();
    // A miss that arrives while the app's refresh is in flight waits for it.
    let pending = reserve(&cache, &app);
    let waiting = match cache.reserve(&app).unwrap() {
        Reservation::Wait(waiting) => waiting,
        _ => panic!("a concurrent miss must wait for the in-flight refresh"),
    };
    let installed = pending.complete(observation(&app)).unwrap();
    let answered = futures::executor::block_on(waiting);
    assert!(
        matches!(answered, Ok(Ok(ref observation)) if observation.same_observation(&installed))
    );
    assert!(serves(&cache, &installed));

    // A cancelled refresh releases its waiter rather than leaving it hanging.
    let cancelled_app = AppId::mint();
    let cancelled = reserve(&cache, &cancelled_app);
    let waiting = match cache.reserve(&cancelled_app).unwrap() {
        Reservation::Wait(waiting) => waiting,
        _ => panic!("a concurrent miss must wait for the in-flight refresh"),
    };
    drop(cancelled);
    assert!(matches!(
        futures::executor::block_on(waiting),
        Ok(Err(Error::Unavailable))
    ));

    assert!(serves(&cache, &peer));
}

/// A waiter whose refresh outlives the read budget is refused inside the
/// budget, never left waiting and never answered from anything but the peer's
/// observation.
///
/// It binds the source's wait arm: a waiter that is not bounded by the read
/// budget would outlive the outer bound here and fail rather than hang.
#[compio::test]
async fn a_waiter_whose_refresh_outlives_the_read_budget_is_refused() {
    let cache = cache(2);
    let app = AppId::mint();
    // The refresh is in flight for the whole budget and never completes.
    let _pending = reserve(&cache, &app);
    let Reservation::Wait(waiting) = cache.reserve(&app).unwrap() else {
        panic!("a concurrent miss must wait for the in-flight refresh");
    };
    let answered = compio::time::timeout(
        Duration::from_secs(5),
        super::super::await_peer_observation(Duration::from_millis(50), waiting),
    )
    .await;
    assert!(
        matches!(answered, Ok(Err(Error::Unavailable))),
        "the waiter must be refused within the read budget: {answered:?}"
    );
}

/// A waiter whose entry is retired before its refresh completes is refused. The
/// cancelled refresh's late result never reaches it.
///
/// It binds the source's cancellation arm: an invalidated entry that kept its
/// waiters would let the late completion answer them with the retired
/// observation instead.
#[compio::test]
async fn a_waiter_is_refused_when_its_refresh_is_retired_before_completion() {
    let cache = cache(2);
    let app = AppId::mint();
    let pending = reserve(&cache, &app);
    let Reservation::Wait(waiting) = cache.reserve(&app).unwrap() else {
        panic!("a concurrent miss must wait for the in-flight refresh");
    };
    // Retiring the entry drops the waiter's sender, and the refresh that was in
    // flight can no longer install or answer anyone.
    cache.invalidate(&app);
    assert!(pending.complete(observation(&app)).is_err());
    let answered = super::super::await_peer_observation(Duration::from_secs(5), waiting).await;
    assert!(
        matches!(answered, Err(Error::Unavailable)),
        "a retired refresh must refuse its waiter, not answer it: {answered:?}"
    );
}

#[test]
fn invalidation_during_refresh_fences_old_completion_and_its_cleanup() {
    let cache = cache(1);
    let app = AppId::mint();
    let pending = reserve(&cache, &app);
    cache.invalidate(&app);
    let replacement = reserve(&cache, &app).complete(observation(&app)).unwrap();
    assert!(matches!(
        pending.complete(observation(&app)),
        Err(Error::Unavailable)
    ));
    assert!(serves(&cache, &replacement));
}

#[test]
fn equal_policy_restoration_never_revives_retired_observation() {
    let cache = cache(1);
    let app = AppId::mint();
    let retired = reserve(&cache, &app).complete(observation(&app)).unwrap();
    cache.invalidate(&app);
    let restored = PolicyObservation::new(
        app.clone(),
        retired.revision(),
        retired.policy().clone(),
        retired.execution_zone_id().clone(),
        retired.deleted(),
        retired.expires_at(),
    )
    .unwrap();
    let restored = reserve(&cache, &app).complete(restored).unwrap();
    assert!(!serves(&cache, &retired));
    assert!(serves(&cache, &restored));
}

#[test]
fn eviction_bounds_entries_and_a_late_result_cannot_recreate_them() {
    let cache = cache(1);
    let app = AppId::mint();
    let peer = AppId::mint();
    let pending = reserve(&cache, &app);
    let current = reserve(&cache, &peer).complete(observation(&peer)).unwrap();
    assert!(pending.complete(observation(&app)).is_err());
    assert_eq!(cache.entries().unwrap().len(), 1);
    assert!(serves(&cache, &current));
    let next = reserve(&cache, &app).complete(observation(&app)).unwrap();
    assert!(serves(&cache, &next));
    // Last: probing the evicted app reserves a refresh for it, which evicts
    // the other.
    assert!(!serves(&cache, &current));
}

#[test]
fn expired_and_foreign_source_results_are_refused() {
    let cache = cache(1);
    let app = AppId::mint();
    assert!(
        reserve(&cache, &app)
            .complete(observation(&AppId::mint()))
            .is_err()
    );
    let mut expired = observation(&app);
    expired.expires_at = Instant::now();
    assert!(reserve(&cache, &app).complete(expired).is_err());
    assert!(cache.entries().unwrap().is_empty());
}

#[compio::test]
async fn expiration_requires_new_authoritative_values_and_never_serves_the_expired_one() {
    let cache = cache(1);
    let app = AppId::mint();
    let deadline = Instant::now() + Duration::from_millis(50);
    let original = PolicyObservation::new(
        app.clone(),
        1.try_into().unwrap(),
        AppPolicy::default(),
        zeroship_core::ZoneId::default_zone(),
        false,
        deadline,
    )
    .unwrap();
    let original = reserve(&cache, &app).complete(original).unwrap();
    compio::time::sleep_until(deadline).await;
    assert!(!serves(&cache, &original));
    let replacement = reserve(&cache, &app).complete(observation(&app)).unwrap();
    assert!(!serves(&cache, &original));
    assert!(serves(&cache, &replacement));
}

/// One manager answers for an app from ONE observation, whichever of its HTTP
/// threads is asked.
///
/// A validity window opens when its observation was read, so a store per thread
/// gives one app as many windows as the manager has threads, at whatever
/// offsets those threads happened to read Control. A worker's leases land on
/// whichever thread accepted the connection, so the deadline it is granted
/// alternates between those windows and moves backwards by their offset - far
/// past the round trip its host allows for re-anchoring, so the host reads it
/// as a shortening and cancels every operation bound to the epoch it holds.
#[test]
fn one_process_observation_answers_every_thread_that_asks() {
    let observations = PolicyObservations::new(NonZeroUsize::new(2).unwrap());
    let app = AppId::mint();
    let installed = reserve(observations.cache(), &app)
        .complete(observation(&app))
        .unwrap();

    let peer = observations.clone();
    let (revision, expires_at) = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let Reservation::Cached(cached) = peer.cache().reserve(&app).unwrap() else {
                    panic!("a thread that did not observe must be served the one that did")
                };
                (cached.revision(), cached.expires_at())
            })
            .join()
            .unwrap()
    });
    assert_eq!(revision, installed.revision());
    assert_eq!(expires_at, installed.expires_at());
    assert!(serves(observations.cache(), &installed));

    // The control: a store of its own holds nothing this one observed, which is
    // what separate stores give and why one manager may not have two.
    let separate = PolicyObservations::new(NonZeroUsize::new(2).unwrap());
    assert!(matches!(
        separate.cache().reserve(&app).unwrap(),
        Reservation::Refresh(_)
    ));
    assert!(!serves(separate.cache(), &installed));
}

/// Nothing a caller holds across its source read carries the map lock.
///
/// A reservation is held across the database read that completes it, and the
/// manager's threads each drive a single-threaded executor: a guard reaching
/// that far would not contend, it would deadlock the thread against its own
/// other tasks. `MutexGuard` is `!Send`, so this stops compiling the moment
/// either value starts carrying one.
#[test]
fn a_reservation_carries_no_lock_across_its_source_read() {
    const fn escapes_no_guard<T: Send>() {}
    escapes_no_guard::<Reservation<'static>>();
    escapes_no_guard::<Ticket<'static>>();
}

fn valid_for(app: &AppId, validity: Duration) -> PolicyObservation {
    PolicyObservation::new(
        app.clone(),
        1.try_into().unwrap(),
        AppPolicy::default(),
        zeroship_core::ZoneId::default_zone(),
        false,
        Instant::now() + validity,
    )
    .unwrap()
}

/// Wait on the monotonic clock until `instant` has passed.
async fn until(instant: Instant) {
    while Instant::now() <= instant {
        compio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// An observation is read again once half its validity has passed, while it is
/// still valid, so no grant is capped by an observation about to expire. Until
/// that point it is served from the cache, and while the read is in flight
/// every other caller keeps it; the completed read replaces it.
#[compio::test]
async fn an_observation_is_read_again_halfway_through_its_validity() {
    let cache = cache(1);
    let app = AppId::mint();
    let validity = Duration::from_millis(400);
    let first = reserve(&cache, &app)
        .complete(valid_for(&app, validity))
        .unwrap();
    let Reservation::Cached(early) = cache.reserve(&app).unwrap() else {
        panic!("an observation inside the first half of its validity is served as it is");
    };
    assert!(early.same_observation(&first));

    until(first.expires_at().checked_sub(validity / 2).unwrap()).await;
    let Reservation::Ahead(ticket, current) = cache.reserve(&app).unwrap() else {
        panic!("an observation past half its validity is read again");
    };
    assert!(current.same_observation(&first));
    assert!(current.expires_at() > Instant::now(), "it is read again while still valid");
    let Reservation::Cached(meanwhile) = cache.reserve(&app).unwrap() else {
        panic!("one read ahead of expiry at a time; the others keep the current observation");
    };
    assert!(meanwhile.same_observation(&first));

    let second = ticket
        .complete(valid_for(&app, Duration::from_secs(30)))
        .unwrap();
    let Reservation::Cached(after) = cache.reserve(&app).unwrap() else {
        panic!("the completed read is served");
    };
    assert!(after.same_observation(&second));
    assert!(second.expires_at() > first.expires_at());
}

/// A read ahead of expiry that does not complete leaves the current
/// observation serving while it is valid, and the next caller reads again.
#[compio::test]
async fn a_failed_read_ahead_of_expiry_keeps_the_current_observation() {
    let cache = cache(1);
    let app = AppId::mint();
    let validity = Duration::from_millis(400);
    let first = reserve(&cache, &app)
        .complete(valid_for(&app, validity))
        .unwrap();
    until(first.expires_at().checked_sub(validity / 2).unwrap()).await;
    let Reservation::Ahead(ticket, _) = cache.reserve(&app).unwrap() else {
        panic!("an observation past half its validity is read again");
    };
    drop(ticket);
    let Reservation::Ahead(_, current) = cache.reserve(&app).unwrap() else {
        panic!("the next caller reads again after a failed read");
    };
    assert!(current.same_observation(&first));
}
