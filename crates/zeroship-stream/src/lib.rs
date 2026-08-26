//! Durable stream transport seam for the billing-provider platform.
//!
//! This abstraction is deliberately Kafka-family shaped: partition offsets are
//! owned by the transport, consumer groups divide partitions among consumers,
//! and a producer-supplied partition key preserves per-subject ordering.

pub mod adapters;
pub mod error;
pub mod registry;
pub mod transport;

pub use error::StreamError;
pub use registry::{StreamConfig, StreamFactory, StreamRegistry};
pub use transport::{StreamOffset, StreamRecord, StreamTransport};
