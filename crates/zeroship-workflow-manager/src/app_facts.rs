//! Control's app and plan facts, read through an injected capability.
//!
//! Two consumers share it: the policy ledger, which publishes one app's
//! authority from these inputs, and the closing lane, which asks which of its
//! candidates Control deleted. They share the row, not the cadence - the
//! ledger reads one app per source-validity window under its publication lock,
//! and the lane reads a page per pass with no lock at all - so the seam is a
//! batch and each caller asks for what it needs.
//!
//! The manager holds no implementation. A host supplies one: the server binds
//! Control's authenticated endpoint, and a test binds whatever it is proving
//! against. That is the severing - there is no database-backed source here to
//! fall back to, so a host that configures nothing gets no facts rather than a
//! silent direct read.
#![expect(
    clippy::future_not_send,
    reason = "Control reads stay on the owning compio runtime"
)]

use crate::Error;
use std::{fmt::Debug, future::Future, pin::Pin};
use zeroship_core::{app_id::AppId, workflow_app_facts::AppFactsResponse};

pub type AppFactsFuture<'a> = Pin<Box<dyn Future<Output = Result<AppFactsResponse, Error>> + 'a>>;

/// Trusted Control facts. Implementations read only authoritative platform
/// state, never anything a worker or creator supplied.
///
/// An unavailable source returns `Unavailable`, a retryable infrastructure
/// failure. An app Control has no row for is ABSENT from the answer, which is
/// not an error: a policy observation refuses such an app on its own account,
/// and a deletion sweep reports only a recorded deletion.
pub trait AppFactsSource: Debug {
    /// Read every named app in one answer, under one watermark.
    ///
    /// An empty slice is refused rather than exchanged, because an empty
    /// answer is indistinguishable from "none of these apps exists".
    fn observe<'a>(&'a self, apps: &'a [AppId]) -> AppFactsFuture<'a>;
}
