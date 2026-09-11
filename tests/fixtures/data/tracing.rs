//! Tracing initialization shared by database integration test binaries.

/// Install test tracing when the external logging filter is configured.
pub fn init_test_tracing() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // `RUST_LOG` is a contract owned by tracing-subscriber, not a zeroship
        // knob, so it is declared `external` with a test-harness consumer
        // marker. Reading it through the typed accessor is what the
        // `disallowed_methods` lint on `std::env::var_os` is asking for; the
        // bare read was a deny-level clippy error that nothing surfaced,
        // because this target only lints under `--all-targets --all-features`.
        if zeroship_core::declared_env_os!(external, "RUST_LOG", zeroship_core::config::TestHarness)
            .is_none()
        {
            return;
        }
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
    });
}
