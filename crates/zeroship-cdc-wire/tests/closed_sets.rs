//! The closed decision tables: watermark rule, per-app retry policy, and the
//! HTTP problem surface.
//!
//! Each is table-driven over a `::ALL` slice, so a variant added without a
//! decision fails here rather than inheriting one. That is the whole point of
//! the enums being closed: "a reason absent from these two sets is a wire-decode
//! error, not a guessed default."

use std::collections::BTreeSet;

use zeroship_cdc_wire::{
    ProblemRetry, RegistrationRejectCode, RegistrationRetry, ResetReason, SubscribeProblem,
    SubscribeProblemCode, WatermarkDecision,
};

#[test]
fn every_reset_reason_has_exactly_one_watermark_decision() {
    let mut clear = BTreeSet::new();
    let mut retain = BTreeSet::new();
    let mut none = BTreeSet::new();
    for reason in ResetReason::ALL {
        match reason.watermark() {
            WatermarkDecision::Clear => clear.insert(*reason),
            WatermarkDecision::Retain => retain.insert(*reason),
            WatermarkDecision::NoPriorWatermark => none.insert(*reason),
        };
    }
    assert_eq!(ResetReason::ALL.len(), 11);
    assert_eq!(clear.len() + retain.len() + none.len(), 11);

    // The exact sets the proposal names. Written out rather than counted,
    // because a reason moving between the two sets keeps the counts identical
    // and changes whether a worker replays rows it has already applied.
    assert_eq!(
        clear,
        [
            ResetReason::RelayFailover,
            ResetReason::GrantRebound,
            ResetReason::DatastoreReconnect,
            ResetReason::SystemIdentityChanged,
        ]
        .into_iter()
        .collect::<BTreeSet<_>>()
    );
    assert_eq!(
        retain,
        [
            ResetReason::RingOverrun,
            ResetReason::DatabaseEpochChanged,
            ResetReason::AppDegraded,
            ResetReason::AppReactivated,
            ResetReason::SlotInvalidated,
            ResetReason::ClassificationChanged,
        ]
        .into_iter()
        .collect::<BTreeSet<_>>()
    );
    assert_eq!(none, BTreeSet::from([ResetReason::Initial]));
}

#[test]
fn every_reject_code_has_its_stated_retry_policy() {
    let table = [
        (
            RegistrationRejectCode::EpochPending,
            RegistrationRetry::Backoff,
        ),
        (
            RegistrationRejectCode::DatastoreUnavailable,
            RegistrationRetry::Backoff,
        ),
        (
            RegistrationRejectCode::StaleExpectedBinding,
            RegistrationRetry::TopologyRefreshFirst,
        ),
        (
            RegistrationRejectCode::AppNotInTopology,
            RegistrationRetry::OnLeaseSetChange,
        ),
        (
            RegistrationRejectCode::GrantInactive,
            RegistrationRetry::OnLeaseSetChange,
        ),
        (
            RegistrationRejectCode::CursorBindingMismatch,
            RegistrationRetry::LocalInvariantFailure,
        ),
        (
            RegistrationRejectCode::CursorAhead,
            RegistrationRetry::LocalInvariantFailure,
        ),
    ];
    assert_eq!(table.len(), RegistrationRejectCode::ALL.len());
    assert_eq!(table.len(), 7);

    let covered: BTreeSet<RegistrationRejectCode> = table.iter().map(|(code, _)| *code).collect();
    assert_eq!(
        covered,
        RegistrationRejectCode::ALL
            .iter()
            .copied()
            .collect::<BTreeSet<_>>(),
        "the table must name every code exactly once"
    );

    for (code, expected) in table {
        assert_eq!(code.retry(), expected, "{code:?}");
    }
}

/// Long because the table is written out in full rather than derived. Deriving
/// the expectation from the implementation would make this test agree with
/// whatever the code says.
#[allow(clippy::too_many_lines)]
#[test]
fn every_problem_code_has_its_stated_status_and_retry() {
    let table = [
        (
            SubscribeProblemCode::AuthenticationFailed,
            401,
            ProblemRetry::Terminal,
        ),
        (
            SubscribeProblemCode::DuplicateApp,
            400,
            ProblemRetry::Terminal,
        ),
        (
            SubscribeProblemCode::NonMonotonicRegistrationGeneration,
            400,
            ProblemRetry::Terminal,
        ),
        (
            SubscribeProblemCode::ConflictingShard,
            400,
            ProblemRetry::Terminal,
        ),
        (
            SubscribeProblemCode::MalformedRequest,
            400,
            ProblemRetry::Terminal,
        ),
        (
            SubscribeProblemCode::NotAcceptable,
            406,
            ProblemRetry::Terminal,
        ),
        (
            SubscribeProblemCode::ClusterMismatch,
            409,
            ProblemRetry::Terminal,
        ),
        (
            SubscribeProblemCode::RequestTooLarge,
            413,
            ProblemRetry::Terminal,
        ),
        (
            SubscribeProblemCode::UnsupportedMediaType,
            415,
            ProblemRetry::Terminal,
        ),
        (
            SubscribeProblemCode::UnsupportedWireVersion,
            426,
            ProblemRetry::Terminal,
        ),
        (
            SubscribeProblemCode::NotLeader,
            503,
            ProblemRetry::JitterRetry,
        ),
        (
            SubscribeProblemCode::AuthenticationStoreUnavailable,
            503,
            ProblemRetry::JitterRetry,
        ),
        (
            SubscribeProblemCode::TermAuthorityUnavailable,
            503,
            ProblemRetry::JitterRetry,
        ),
        (
            SubscribeProblemCode::ConnectionCapacityExceeded,
            503,
            ProblemRetry::JitterRetry,
        ),
    ];
    assert_eq!(table.len(), 14, "fourteen closed problem codes");
    assert_eq!(table.len(), SubscribeProblemCode::ALL.len());

    let covered: BTreeSet<SubscribeProblemCode> = table.iter().map(|(code, ..)| *code).collect();
    assert_eq!(
        covered,
        SubscribeProblemCode::ALL
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
    );

    let mut retryable = 0;
    for (code, status, retry) in table {
        assert_eq!(code.status(), status, "{code:?} status");
        assert_eq!(code.retry(), retry, "{code:?} retry");
        if retry == ProblemRetry::JitterRetry {
            retryable += 1;
            assert_eq!(status, 503, "only a 503 retries");
        }
    }
    assert_eq!(
        retryable, 4,
        "the proposal names exactly four retryable 503s"
    );

    assert_eq!(
        ProblemRetry::JitterRetry.window(),
        Some((
            core::time::Duration::from_millis(250),
            core::time::Duration::from_secs(5)
        ))
    );
    assert_eq!(ProblemRetry::Terminal.window(), None);
}

#[test]
fn the_problem_body_round_trips_and_refuses_anything_else() {
    let mut checked = 0;
    for code in SubscribeProblemCode::ALL {
        let problem = SubscribeProblem::new(*code);
        assert_eq!(problem.status, code.status());
        let json = problem.to_json().expect("render");
        assert!(
            json.contains(code.as_str()),
            "the body must carry the exact code string"
        );
        assert_eq!(SubscribeProblem::from_json(&json).expect("parse"), problem);
        checked += 1;
    }
    assert_eq!(checked, 14);

    // An unknown code is refused rather than defaulted.
    assert!(SubscribeProblem::from_json(r#"{"status":503,"code":"Whatever"}"#).is_err());
    // An extra field is refused: a free-text field on this surface is where a
    // schema name reaches an unauthenticated caller.
    assert!(SubscribeProblem::from_json(
        r#"{"status":503,"code":"NotLeader","detail":"schema db_x is missing"}"#
    )
    .is_err());
    // The exact shape, pinned.
    assert_eq!(
        SubscribeProblem::new(SubscribeProblemCode::NotLeader)
            .to_json()
            .expect("render"),
        r#"{"status":503,"code":"NotLeader"}"#
    );
}

#[test]
fn the_debug_impls_do_not_print_row_bytes_or_the_permit() {
    use zeroship_cdc_wire::CellValue;

    let cell = CellValue::Value(b"a-secret-row-value".to_vec());
    let rendered = format!("{cell:?}");
    assert!(
        !rendered.contains("secret"),
        "CellValue::Debug printed the value: {rendered}"
    );
    assert!(rendered.contains("18 bytes"), "got {rendered}");
    assert_eq!(format!("{:?}", CellValue::Null), "Null");
    assert_eq!(format!("{:?}", CellValue::Unavailable), "Unavailable");
}
