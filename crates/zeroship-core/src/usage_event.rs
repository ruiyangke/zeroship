//! Shared billing usage-event wire types.
//!
//! These are inter-service values: workers publish them to the durable stream,
//! and control consumes the same JSON payload before forwarding to a billing
//! provider. Keep provider-local rating/invoice types out of this module.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The app/creator subject stamped by the trusted runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSubject {
    /// The app that consumed the metered resource. Legacy creator-level export
    /// events may omit this until the worker producer slice lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<Uuid>,
    pub creator: Uuid,
}

/// One immutable usage event on the billing-provider stream.
///
/// The field names are the stable JSON contract. The shape is deliberately close
/// to CloudEvents: `event_id` is the idempotency key, `source` namespaces it,
/// `subject` carries app/creator attribution, and `event_time` is the event's
/// Unix timestamp in seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageEvent {
    pub event_id: String,
    pub source: String,
    pub subject: UsageSubject,
    pub meter: String,
    pub value: u64,
    pub event_time: i64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dims: BTreeMap<String, String>,
}

impl UsageEvent {
    /// Provider-facing default subject for adapters that meter at creator grain.
    #[must_use]
    pub fn creator_subject(&self) -> String {
        self.subject.creator.to_string()
    }
}
