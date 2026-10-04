use super::*;

const BACKOFF: Duration = Duration::from_secs(1);

/// Whether `app` is excluded at `elapsed` after `start`.
fn excluded_at(skipped: &mut SkipList, app: &AppId, start: Instant, elapsed: Duration) -> bool {
    skipped.excluded(start + elapsed, BACKOFF).contains(app)
}

/// Each consecutive failure doubles the time an app stays excluded, until the
/// ceiling: the app is excluded just before each expiry and not at it.
#[test]
fn an_expiry_doubles_per_consecutive_failure_up_to_its_ceiling() {
    let app = AppId::mint();
    let start = Instant::now();
    let mut skipped = SkipList::default();
    let mut expiries = Vec::new();
    for _ in 0..9 {
        skipped.failed(&app, start, BACKOFF);
        let expiry = (1..=128)
            .map(Duration::from_secs)
            .find(|elapsed| !excluded_at(&mut skipped, &app, start, *elapsed))
            .expect("the entry expires within its ceiling");
        assert!(excluded_at(
            &mut skipped,
            &app,
            start,
            expiry.checked_sub(Duration::from_millis(1)).unwrap()
        ));
        expiries.push(expiry.as_secs());
    }
    assert_eq!(expiries, [1, 2, 4, 8, 16, 32, 64, 64, 64]);
}

/// A successful preparation ends the run: the next failure starts over.
#[test]
fn a_successful_preparation_ends_the_run_of_failures() {
    let app = AppId::mint();
    let start = Instant::now();
    let mut skipped = SkipList::default();
    skipped.failed(&app, start, BACKOFF);
    skipped.failed(&app, start, BACKOFF);
    assert!(excluded_at(&mut skipped, &app, start, BACKOFF));
    skipped.prepared(&app);
    assert!(!excluded_at(&mut skipped, &app, start, Duration::ZERO));
    skipped.failed(&app, start, BACKOFF);
    assert!(excluded_at(&mut skipped, &app, start, BACKOFF / 2));
    assert!(!excluded_at(&mut skipped, &app, start, BACKOFF));
}

/// An entry quiet for a whole ceiling past its expiry is forgotten, so a later
/// failure starts the doubling over; one failing again sooner keeps it.
#[test]
fn an_entry_quiet_for_a_whole_ceiling_is_forgotten() {
    let start = Instant::now();
    let ceiling = skip_ceiling(BACKOFF);
    for (quiet, expected) in [(ceiling, BACKOFF), (ceiling / 2, BACKOFF * 4)] {
        let app = AppId::mint();
        let mut skipped = SkipList::default();
        skipped.failed(&app, start, BACKOFF);
        skipped.failed(&app, start, BACKOFF);
        let expired = start + BACKOFF * 2;
        let later = expired + quiet;
        assert!(skipped.excluded(later, BACKOFF).is_empty());
        skipped.failed(&app, later, BACKOFF);
        assert!(skipped
            .excluded(
                (later + expected)
                    .checked_sub(Duration::from_millis(1))
                    .unwrap(),
                BACKOFF,
            )
            .contains(&app));
        assert!(!skipped.excluded(later + expected, BACKOFF).contains(&app));
    }
}

/// A claim excludes at most its limit, keeping the entries that expire last.
#[test]
fn exclusion_sends_the_entries_that_expire_last_up_to_its_limit() {
    let start = Instant::now();
    let mut skipped = SkipList::default();
    let soonest = AppId::mint();
    skipped.failed(&soonest, start, BACKOFF);
    let mut later = Vec::new();
    for _ in 0..ClaimJobs::MAX_EXCLUDE {
        let app = AppId::mint();
        skipped.failed(&app, start, BACKOFF);
        skipped.failed(&app, start, BACKOFF);
        later.push(app);
    }
    let mut excluded = skipped.excluded(start, BACKOFF);
    assert_eq!(excluded.len(), ClaimJobs::MAX_EXCLUDE);
    assert!(!excluded.contains(&soonest));
    excluded.sort();
    later.sort();
    assert_eq!(excluded, later);
}

#[test]
fn consumer_options_refuse_bounds_they_cannot_represent() {
    let valid = ConsumerOptions {
        slots: 1,
        idle_poll: Duration::from_millis(5),
        error_backoff: BACKOFF,
        drain: Duration::ZERO,
        delivery: DeliveryOptions {
            execution_timeout: Duration::from_secs(1),
            operation_timeout: Duration::from_secs(1),
            retry_delay: Duration::from_millis(5),
        },
    };
    valid.validate().unwrap();
    // The back-off the ceiling case uses is one a deadline can hold.
    assert!(Instant::now()
        .checked_add(Duration::from_secs(u64::MAX >> SKIP_DOUBLINGS))
        .is_some());
    for invalid in [
        ConsumerOptions { slots: 0, ..valid },
        ConsumerOptions {
            idle_poll: Duration::ZERO,
            ..valid
        },
        ConsumerOptions {
            error_backoff: Duration::ZERO,
            ..valid
        },
        // Representable itself, but its skip ceiling is not.
        ConsumerOptions {
            error_backoff: Duration::from_secs(u64::MAX >> SKIP_DOUBLINGS),
            ..valid
        },
        ConsumerOptions {
            drain: Duration::MAX,
            ..valid
        },
    ] {
        assert!(matches!(
            invalid.validate(),
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
    }
}

/// A claim asks for the free slots up to the protocol's bound, however many
/// slots the host runs; the control is a host below the bound, which asks for
/// exactly its free slots.
#[test]
fn a_claim_asks_for_no_more_than_the_protocol_allows() {
    let options = ConsumerOptions {
        slots: 1,
        idle_poll: Duration::from_millis(5),
        error_backoff: BACKOFF,
        drain: Duration::ZERO,
        delivery: DeliveryOptions {
            execution_timeout: Duration::from_secs(1),
            operation_timeout: Duration::from_secs(1),
            retry_delay: Duration::from_millis(5),
        },
    };
    let skipped = RefCell::new(SkipList::default());
    let bound = usize::try_from(ClaimJobs::MAX_DELIVERIES).unwrap();
    for (free, asked) in [(bound * 4, ClaimJobs::MAX_DELIVERIES), (3, 3)] {
        let request = claim_request(free, None, &skipped, options).unwrap();
        assert_eq!(request.max.get(), asked, "{free} free slots");
    }
}
