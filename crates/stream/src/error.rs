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

    #[error("kafka error: {0}")]
    Kafka(String),
}

impl From<rdkafka::error::KafkaError> for StreamError {
    fn from(value: rdkafka::error::KafkaError) -> Self {
        Self::Kafka(value.to_string())
    }
}

impl From<serde_json::Error> for StreamError {
    fn from(value: serde_json::Error) -> Self {
        Self::Config(value.to_string())
    }
}
