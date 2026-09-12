//! Shared change delivery for ORM callers and database capture adapters.
//!
//! Embedded SQLite capture and the PostgreSQL relay feed the same broker.
//! The relay service owns PostgreSQL replication. Subscription matching and delivery do not depend on V8.

pub mod broker;
mod event;
pub mod read_set;
mod source;

pub use broker::{Subscription, SubscriptionMessage};
pub use event::{ChangeEvent, ChangeOp};
pub use source::{ChangeSink, DeliveryDisposition};

pub mod relay;

pub mod lifecycle;
