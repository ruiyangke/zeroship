//! Shared billing usage-event wire types.
//!
//! These are inter-service values: workers publish them to the durable stream,
//! and control consumes the same JSON payload before forwarding to a billing
//! provider. Keep provider-local rating/invoice types out of this module.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::app_id::AppId;

/// The app/organization subject stamped by the trusted runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSubject {
    /// The app that consumed the metered resource. An export event emitted at
    /// organization grain may omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppId>,
    /// The organization billed for this usage (`org_…`).
    ///
    /// `None` on the wire the WORKER writes, and that is not a defect: the
    /// worker holds only the server-injected `app_id`, and the app -> owning
    /// organization mapping is the control plane's. The forwarder fills it in
    /// (`cron::event_forwarder`) and dead-letters an event it cannot attribute.
    ///
    /// It is an `Option`, not a sentinel, because the previous shape used
    /// `Uuid::nil()` for the same "not yet resolved" state — a value the type
    /// system could not distinguish from a real subject, and which a provider
    /// that skipped the forwarder would have billed as one customer for the
    /// whole fleet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
}

/// One immutable usage event on the billing-provider stream.
///
/// The field names are the stable JSON contract. The shape is deliberately close
/// to CloudEvents: `event_id` is the idempotency key, `source` namespaces it,
/// `subject` carries app/organization attribution, and `event_time` is the
/// event's Unix timestamp in seconds.
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
    /// Provider-facing subject for adapters that meter at organization grain.
    ///
    /// `None` for an event the forwarder has not yet attributed. Every caller is
    /// downstream of that attribution, so `None` here means an unattributed
    /// event reached a provider and the call must fail rather than invent a
    /// customer.
    #[must_use]
    pub fn organization_subject(&self) -> Option<&str> {
        self.subject.organization.as_deref()
    }
}
