use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use zeroship_stream::{
    StreamConfig, StreamError, StreamOffset, StreamRecord, StreamRegistry, StreamTransport,
};

#[derive(Debug)]
struct FakeTransport;

#[async_trait(?Send)]
impl StreamTransport for FakeTransport {
    fn id(&self) -> &str {
        "fake"
    }

    async fn publish(
        &self,
        _topic: &str,
        _partition_key: &[u8],
        _payload: &[u8],
    ) -> Result<(), StreamError> {
        Ok(())
    }

    async fn poll(&self, _max: usize) -> Result<Vec<StreamRecord>, StreamError> {
        Ok(Vec::new())
    }

    async fn commit(&self, _offsets: &[StreamOffset]) -> Result<(), StreamError> {
        Ok(())
    }

    async fn rewind(&self) -> Result<(), StreamError> {
        Ok(())
    }
}

fn fake_factory(config: &StreamConfig) -> Result<Arc<dyn StreamTransport>, StreamError> {
    #[derive(Deserialize)]
    struct FakeConfig {
        #[serde(default)]
        ok: bool,
    }

    let require_ok = config.parse::<FakeConfig>()?.ok;
    if !require_ok {
        return Err(StreamError::Config("fake: ok=true required".into()));
    }
    Ok(Arc::new(FakeTransport))
}

#[test]
fn registry_register_build_known_and_fail_closed_unknown() {
    let mut registry = StreamRegistry::default();
    registry.register("fake", fake_factory);

    assert_eq!(registry.known(), vec!["fake"]);

    let transport = registry
        .build("fake", &StreamConfig::from(json!({ "ok": true })))
        .expect("fake transport builds from registered factory");
    assert_eq!(transport.id(), "fake");

    let err = registry
        .build("missing", &StreamConfig::default())
        .err()
        .expect("unknown transports fail closed");
    assert!(matches!(err, StreamError::UnknownTransport { .. }));
    assert!(err.to_string().contains("known: fake"));

    let err = registry
        .build("fake", &StreamConfig::default())
        .err()
        .expect("factory config validation is surfaced");
    assert!(err.to_string().contains("ok=true required"));
}
