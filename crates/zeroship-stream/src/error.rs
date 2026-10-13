use rdkafka::error::{KafkaError, RDKafkaErrorCode};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StreamError {
    #[error("stream config error: {0}")]
    Config(String),

    #[error("unknown stream transport '{id}' (known: {known})")]
    UnknownTransport { id: String, known: String },

    #[error("stream transport '{0}' is already registered")]
    DuplicateTransport(&'static str),

    #[error("stream transport '{0}' is unavailable")]
    Unavailable(&'static str),

    #[error("stream publish timed out after {timeout_ms}ms")]
    PublishTimeout { timeout_ms: u64 },

    /// librdkafka evicted this consumer from its group because the application
    /// went longer than `max.poll.interval.ms` between `poll` calls - the shape
    /// of a consumer that is kept alive across cycles and left idle between
    /// them, as the control spend recompute's is. librdkafka queues this once
    /// when it leaves the group; the following polls rejoin and the group
    /// re-delivers from the reset point, so a caller reading a finite range
    /// treats this as the expected rejoin and keeps polling. Every other Kafka
    /// failure stays [`Self::Kafka`], so a caller that special-cases this one
    /// still fails on a broker that is genuinely broken.
    #[error(
        "kafka consumer exceeded max.poll.interval.ms and left its group; \
         rejoin by polling"
    )]
    PollExceeded,

    #[error("kafka error: {0}")]
    Kafka(String),
}

impl From<KafkaError> for StreamError {
    fn from(value: KafkaError) -> Self {
        // librdkafka reports a too-slow poll as the local `__MAX_POLL_EXCEEDED`
        // code on `MessageConsumption`, the same event branch a genuine
        // consumption failure arrives on. The variant and the code together
        // tell them apart: only that pair is the rejoin signal, so a fatal
        // consumption error carrying the same code, and every other code,
        // keeps its generic identity and still fails the caller.
        match value {
            KafkaError::MessageConsumption(RDKafkaErrorCode::PollExceeded) => Self::PollExceeded,
            other => Self::Kafka(other.to_string()),
        }
    }
}

impl From<serde_json::Error> for StreamError {
    fn from(value: serde_json::Error) -> Self {
        Self::Config(value.to_string())
    }
}
