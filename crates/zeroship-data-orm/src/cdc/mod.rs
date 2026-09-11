//! Shared change delivery for ORM callers and database capture adapters.
//!
//! Local SQLite capture and worker PostgreSQL capture feed the same broker.
//! Relay wire frames and PostgreSQL replication ownership remain outside this
//! module. Subscription matching and delivery do not depend on V8.

pub mod broker;
mod event;
pub mod read_set;
mod source;

pub use broker::{Subscription, SubscriptionMessage};
pub use event::{ChangeEvent, ChangeOp};
pub use source::{ChangeSink, ChangeStream, DeliveryDisposition};

pub mod relay;
