use zeroship_kv::{KvConfig, Topology};

#[test]
fn runtime_configuration_selects_each_topology() {
    for (mode, details) in [
        ("standalone", "endpoint = 'localhost:6379'"),
        ("cluster", "seeds = ['redis-a:6379', 'redis-b:6379']"),
        (
            "sentinel",
            "endpoints = ['sentinel-a:26379']\nservice_name = 'kv'",
        ),
    ] {
        let config = KvConfig::from_toml(&format!(
            "backend = 'redis'\n[redis.topology]\nmode = '{mode}'\n{details}"
        ))
        .unwrap();
        let KvConfig::Redis { redis } = config else {
            panic!("expected Redis")
        };
        assert!(matches!(
            (&redis.topology, mode),
            (Topology::Standalone { .. }, "standalone")
                | (Topology::Cluster { .. }, "cluster")
                | (Topology::Sentinel { .. }, "sentinel")
        ));
    }
    assert!(matches!(
        KvConfig::from_toml("backend = 'redb'\npath = 'state/kv.redb'").unwrap(),
        KvConfig::Redb { .. }
    ));
}

#[test]
fn invalid_and_conflicting_settings_fail_before_connecting() {
    for input in [
        "backend = 'redis'",
        "backend = 'redis'\n[redis.topology]\nmode = 'sentinal'",
        "backend = 'redis'\n[redis.topology]\nmode = 'standalone'\nendpoint = 'redis:6379'\nseeds = ['other:6379']",
        "backend = 'redis'\n[redis.topology]\nmode = 'cluster'\nseeds = []",
        "backend = 'redis'\n[redis]\ndatabase = 1\n[redis.topology]\nmode = 'cluster'\nseeds = ['redis:6379']",
        "backend = 'redis'\n[redis.topology]\nmode = 'sentinel'\nservice_name = ''\nendpoints = ['sentinel:26379']",
        "backend = 'redis'\n[redis.topology]\nmode = 'standalone'\nendpoint = 'user:secret@redis:6379'",
        "backend = 'redis'\n[redis.topology]\nmode = 'standalone'\nendpoint = 'redis:6379'\n[redis.pool]\nmax_size = 0",
        "backend = 'redis'\n[redis.topology]\nmode = 'standalone'\nendpoint = 'redis:6379'\n[redis.timeouts]\nconnect_ms = 0",
        "backend = 'redis'\n[redis.topology]\nmode = 'standalone'\nendpoint = 'redis:6379'\n[redis.auth]\nusername = 'kv'",
        "backend = 'redis'\n[redis.topology]\nmode = 'standalone'\nendpoint = 'redis:6379'\n[redis.tls]\ncert_file = 'cert.pem'",
        "backend = 'redis'\n[redis.topology]\nmode = 'standalone'\nendpoint = 'redis:6379'\n[redis.sentinel_auth]\npassword = 'secret'",
    ] {
        let error = KvConfig::from_toml(input).unwrap_err();
        assert!(!error.to_string().contains("secret"));
    }
}

#[test]
fn authentication_is_separate_for_sentinel_and_data_servers_and_redacted() {
    let config = KvConfig::from_toml("backend = 'redis'\n[redis.topology]\nmode = 'sentinel'\nendpoints = ['sentinel:26379']\nservice_name = 'kv'\n[redis.auth]\nusername = 'data-user'\npassword = 'data-secret'\n[redis.sentinel_auth]\nusername = 'sentinel-user'\npassword = 'sentinel-secret'").unwrap();
    assert!(!format!("{config:?}").contains("secret"));
    let KvConfig::Redis { redis } = config else {
        panic!("expected Redis")
    };
    assert_eq!(redis.auth.password.as_deref(), Some("data-secret"));
    assert_eq!(
        redis.sentinel_auth.password.as_deref(),
        Some("sentinel-secret")
    );
    assert!(!format!("{redis:?}").contains("secret"));
}
