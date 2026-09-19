//! Both storage boundaries reduce a `DbError` to a closed outcome, and the
//! retryable outcome is the only one a caller may repeat.
//!
//! `DbError` is the input of these conversions, so each case builds the
//! variant it is about. The retryable arm is a disjunction: every variant in
//! it is a separate claim, so each gets its own case and a caller cannot lose
//! one without a named failure. Each boundary also carries the refusals that
//! must stay terminal, because a lost arm falls through to the catch-all and
//! still returns an error, just one nothing retries.

use zeroship_data_orm::error::DbError;
use zeroship_workflow_manager::{deployments, Error};

fn transient() -> DbError {
    DbError::Transient {
        message: "connection reset by peer".to_owned(),
    }
}

fn serialization() -> DbError {
    DbError::Serialization {
        message: "could not serialize access due to read/write dependencies".to_owned(),
    }
}

fn lock_contention() -> DbError {
    DbError::LockContention {
        message: "could not obtain lock on row".to_owned(),
    }
}

fn unique_violation() -> DbError {
    DbError::UniqueViolation {
        message: "duplicate key value violates unique constraint".to_owned(),
    }
}

fn refused(code: &'static str) -> DbError {
    DbError::SchemaRefused {
        code,
        envelope_json: r#"{"code":"refused"}"#.to_owned(),
    }
}

const fn permission_denied() -> DbError {
    DbError::PermissionDenied {
        code: "grant_revoked",
        message: "the database grant has been revoked",
    }
}

fn unclassified() -> DbError {
    DbError::Internal {
        message: "an unclassified backend failure".to_owned(),
    }
}

#[test]
fn the_manager_retries_a_transient_failure() {
    assert_eq!(Error::from(transient()), Error::Unavailable);
}

#[test]
fn the_manager_retries_a_serialization_failure() {
    assert_eq!(Error::from(serialization()), Error::Unavailable);
}

#[test]
fn the_manager_retries_lock_contention() {
    assert_eq!(Error::from(lock_contention()), Error::Unavailable);
}

#[test]
fn the_manager_reports_a_unique_violation_as_a_conflict() {
    assert_eq!(Error::from(unique_violation()), Error::Conflict);
}

#[test]
fn the_manager_reports_a_refused_unique_violation_as_a_conflict() {
    assert_eq!(Error::from(refused("unique_violation")), Error::Conflict);
}

#[test]
fn the_manager_holds_another_refusal_short_of_a_conflict() {
    assert_eq!(Error::from(refused("check_violation")), Error::Storage);
}

#[test]
fn the_manager_keeps_a_permission_refusal_terminal() {
    assert_eq!(Error::from(permission_denied()), Error::Storage);
}

#[test]
fn the_manager_keeps_an_unclassified_failure_terminal() {
    assert_eq!(Error::from(unclassified()), Error::Storage);
}

#[test]
fn deployment_retention_retries_a_transient_failure() {
    assert!(matches!(
        deployments::Error::from(transient()),
        deployments::Error::Unavailable(_)
    ));
}

#[test]
fn deployment_retention_retries_a_serialization_failure() {
    assert!(matches!(
        deployments::Error::from(serialization()),
        deployments::Error::Unavailable(_)
    ));
}

#[test]
fn deployment_retention_retries_lock_contention() {
    assert!(matches!(
        deployments::Error::from(lock_contention()),
        deployments::Error::Unavailable(_)
    ));
}

#[test]
fn deployment_retention_refuses_a_permission_denial() {
    assert!(matches!(
        deployments::Error::from(permission_denied()),
        deployments::Error::PermissionDenied
    ));
}

#[test]
fn deployment_retention_keeps_a_unique_violation_terminal() {
    assert!(matches!(
        deployments::Error::from(unique_violation()),
        deployments::Error::Internal(_)
    ));
}

#[test]
fn deployment_retention_keeps_a_refused_unique_violation_terminal() {
    assert!(matches!(
        deployments::Error::from(refused("unique_violation")),
        deployments::Error::Internal(_)
    ));
}

#[test]
fn deployment_retention_keeps_an_unclassified_failure_terminal() {
    assert!(matches!(
        deployments::Error::from(unclassified()),
        deployments::Error::Internal(_)
    ));
}
