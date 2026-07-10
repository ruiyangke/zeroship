//! Drift guard: the vendored `zeroship_migrate::id` / `zeroship_migrate::db_url`
//! copies must stay byte-identical to their `zeroship_core` originals while both
//! crates coexist in-tree.
//!
//! The base62/UUID encoding and the SQLite-DSN classifier are wire contracts; a
//! silent divergence between core and the vendored copy would corrupt migration
//! ids or misroute a backend. These tests are DB-free and must always pass.

use uuid::Uuid;

/// A representative set of UUIDs: the two extremes plus three hard-coded values.
fn uuid_vectors() -> Vec<Uuid> {
    vec![
        Uuid::nil(),
        Uuid::from_bytes([0xff; 16]),
        Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
        Uuid::parse_str("018f3a2b-1c4d-7e6f-8a9b-0c1d2e3f4a5b").unwrap(),
        Uuid::parse_str("deadbeef-cafe-babe-f00d-0123456789ab").unwrap(),
    ]
}

#[test]
fn uuid_to_base62_matches_core() {
    for u in uuid_vectors() {
        assert_eq!(
            zeroship_migrate::id::uuid_to_base62(&u),
            zeroship_core::typed_id::uuid_to_base62(&u),
            "uuid_to_base62 drift for {u}"
        );
    }
}

#[test]
fn base62_encode_bytes_matches_core() {
    let twenty: Vec<u8> = (0u8..20).collect();
    let vectors: Vec<&[u8]> = vec![
        &[],
        &[0u8],
        &[0xffu8],
        &twenty,
    ];
    for bytes in vectors {
        assert_eq!(
            zeroship_migrate::id::base62_encode_bytes(bytes),
            zeroship_core::typed_id::base62_encode_bytes(bytes),
            "base62_encode_bytes drift for {bytes:?}"
        );
    }
}

#[test]
fn is_sqlite_url_matches_core() {
    // Cover every branch of core's classifier: empty / whitespace, :memory:,
    // postgres schemes (case-insensitive), sqlite/file schemes, bare paths,
    // and explicit-but-unknown schemes.
    let urls = [
        "sqlite:foo.db",
        "sqlite://./data/app.sqlite",
        "file:foo.db",
        ":memory:",
        "postgres://x",
        "postgresql://x",
        "POSTGRES://Localhost/dev",
        "mysql://x",
        "redis://localhost:6379",
        "./foo.db",
        "/var/lib/zeroship/dev.sqlite",
        "",
        "   ",
        "SQLITE:UPPER.db",
    ];
    for u in urls {
        assert_eq!(
            zeroship_migrate::db_url::is_sqlite_url(u),
            zeroship_core::db_url::is_sqlite_url(u),
            "is_sqlite_url drift for {u:?}"
        );
    }
}
