//! What the startup exchange SETTLED ON, asked of a real server.
//!
//! This crate requests protocol 3.2 by default and falls back to 3.0 when the
//! server answers `NegotiateProtocolVersion`. Nothing could observe which
//! happened until `Client::protocol_version` existed, so the negotiation was
//! believed rather than asserted - and PostgreSQL offers no server-side view
//! to check against (`pg_stat_activity` has no such column, `pg_settings`
//! carries only the TLS versions).
//!
//! These run against whatever `PG_TEST_URL` names, so the EXPECTATION comes
//! from the server's own version rather than being hardcoded. That is what
//! lets one test body assert the fallback on 15/16 and the negotiation on 18.

#[allow(unused_imports)]
use crate::common;
use common::{suite_tls, test_url};
use compio_postgres::Config;
use compio_postgres::config::ProtocolVersion;

async fn connect() -> compio_postgres::Client {
    let config: Config = test_url().parse().expect("the suite DSN parses");
    let (client, connection) = config
        .connect(suite_tls())
        .await
        .expect("connect to the suite server");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

#[compio::test]
async fn the_negotiated_version_matches_what_the_server_can_speak() {
    let client = connect().await;

    let server: String = client
        .query_one_scalar("SELECT current_setting('server_version_num')", &[])
        .await
        .expect("ask the server its version");
    let server_num: i32 = server.parse().expect("server_version_num is a number");

    let negotiated = client.protocol_version();
    let expected = if server_num >= 180_000 {
        ProtocolVersion::V3_2
    } else {
        ProtocolVersion::V3_0
    };

    assert_eq!(
        negotiated, expected,
        "server_version_num={server_num} speaks {expected:?}, but the session \
         settled on {negotiated:?}"
    );
}

/// The CONTROL. Asking for 3.0 must give 3.0 even where 3.2 is available, or
/// the test above would pass for a driver that reports a constant.
#[compio::test]
async fn requesting_3_0_settles_on_3_0_even_where_3_2_is_available() {
    let mut config: Config = test_url().parse().expect("the suite DSN parses");
    config.max_protocol_version(ProtocolVersion::V3_0);

    let (client, connection) = config
        .connect(suite_tls())
        .await
        .expect("connect requesting 3.0");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    assert_eq!(client.protocol_version(), ProtocolVersion::V3_0);
}
